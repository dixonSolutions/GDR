/**
 * GDR Windows — the window plane for gdrd.
 *
 * Why an extension at all: GNOME 50's `org.gnome.Shell.Introspect.GetWindows`
 * answers `AccessDenied` to everything except the desktop portal, and the
 * `org.gnome.shell introspect` GSetting that used to open it up no longer
 * exists. Mutter's RemoteDesktop/ScreenCast APIs that gdrd already speaks show
 * *pixels*, never *windows*. So a shell extension is the only supported way for
 * a daemon in the same session to enumerate windows or activate one.
 *
 * This exports `org.gdr.Windows` on the **session bus**, which means it is
 * reachable by anything running as this user — same trust level as gdrd's own
 * D-Bus usage. It deliberately exposes no way to read window *contents*; that
 * still goes through ScreenCast, with its own token scope.
 *
 * Coordinates here are GNOME **stage/logical** pixels. gdrd converts them to
 * capture-stream (native) pixels, because only gdrd knows which connector it
 * is recording and at what size. Do not scale anything in this file.
 */

import Gio from "gi://Gio";
import GLib from "gi://GLib";
import Meta from "gi://Meta";
import Shell from "gi://Shell";

import { Extension } from "resource:///org/gnome/shell/extensions/extension.js";
import * as Main from "resource:///org/gnome/shell/ui/main.js";

const BUS_NAME = "org.gdr.Windows";
const OBJECT_PATH = "/org/gdr/Windows";

/** Bump when the JSON payload shape changes in a way gdrd must notice. */
const API_VERSION = 1;

/**
 * Events kept in memory for pollers that were not listening at the time.
 *
 * gdrd polls this; a poller that falls further behind than this loses the
 * oldest events and is told so via `dropped` rather than being handed a
 * silently incomplete history.
 */
const EVENT_RING = 512;

const IFACE = `
<node>
  <interface name="org.gdr.Windows">
    <method name="List">
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="Act">
      <arg type="t" direction="in" name="id"/>
      <arg type="s" direction="in" name="action"/>
      <arg type="s" direction="in" name="args_json"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="Launch">
      <arg type="s" direction="in" name="app_id"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="ListApps">
      <arg type="s" direction="in" name="filter"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="Events">
      <arg type="t" direction="in" name="since"/>
      <arg type="u" direction="in" name="limit"/>
      <arg type="s" direction="out" name="json"/>
    </method>
    <property name="ApiVersion" type="u" access="read"/>
    <signal name="Changed">
      <arg type="s" name="kind"/>
      <arg type="t" name="id"/>
    </signal>
  </interface>
</node>`;

function rectOf(r) {
  if (!r) return null;
  return { x: r.x, y: r.y, width: r.width, height: r.height };
}

function windowTypeName(type) {
  for (const [name, value] of Object.entries(Meta.WindowType)) {
    if (value === type) return name.toLowerCase();
  }
  return String(type);
}

/**
 * Read a window's maximize state across shell versions.
 *
 * Mutter 50 (GNOME 50) renamed `Meta.Window.get_maximized()` to `get_maximize_flags()`.
 * The extension declares support back to GNOME 45, so probe rather than pick one: on 50
 * the old name is simply absent and calling it throws
 * "win.get_maximized is not a function", which took the whole window listing down.
 */
function maximizeFlagsOf(win) {
  if (typeof win.get_maximize_flags === "function") return win.get_maximize_flags();
  if (typeof win.get_maximized === "function") return win.get_maximized();
  let flags = 0;
  if (win.maximized_horizontally) flags |= Meta.MaximizeFlags.HORIZONTAL;
  if (win.maximized_vertically) flags |= Meta.MaximizeFlags.VERTICAL;
  return flags;
}

function maximizedName(flags) {
  if (flags === Meta.MaximizeFlags.BOTH) return "both";
  if (flags === Meta.MaximizeFlags.HORIZONTAL) return "horizontal";
  if (flags === Meta.MaximizeFlags.VERTICAL) return "vertical";
  return "none";
}

/** Every Meta.Window the compositor is managing, newest last. */
function allWindows() {
  if (typeof global.display.list_all_windows === "function") {
    return global.display.list_all_windows();
  }
  return global.get_window_actors().map((a) => a.meta_window);
}

export default class GdrWindowsExtension extends Extension {
  enable() {
    this._seq = 0;
    this._events = [];
    this._signals = [];
    this._unmanaged = new Map();

    this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE, this);
    this._dbus.export(Gio.DBus.session, OBJECT_PATH);
    this._ownerId = Gio.bus_own_name(
      Gio.BusType.SESSION,
      BUS_NAME,
      Gio.BusNameOwnerFlags.REPLACE,
      null,
      null,
      null
    );

    this._connect(global.display, "window-created", (_d, win) => {
      this._track(win);
      this._push("opened", win);
    });
    this._connect(global.display, "notify::focus-window", () => {
      const win = global.display.focus_window;
      if (win) this._push("focused", win);
    });
    // Nice-to-have events, connected defensively: these ShellWM signals have
    // come and gone across releases, and a throw here would leave the whole
    // window plane dead rather than merely missing minimize events — which
    // the caller can still see by re-listing.
    this._connectOptional(global.window_manager, "minimize", (_wm, actor) =>
      this._push("minimized", actor.meta_window)
    );
    this._connectOptional(global.window_manager, "unminimize", (_wm, actor) =>
      this._push("unminimized", actor.meta_window)
    );

    // Windows that already existed when we were enabled still need an
    // `unmanaged` hook, or their close would go unreported.
    for (const win of allWindows()) this._track(win);
  }

  disable() {
    for (const [obj, id] of this._signals) {
      try {
        obj.disconnect(id);
      } catch {
        /* object already gone */
      }
    }
    this._signals = [];

    for (const [win, id] of this._unmanaged) {
      try {
        win.disconnect(id);
      } catch {
        /* window already gone */
      }
    }
    this._unmanaged.clear();

    if (this._ownerId) {
      Gio.bus_unown_name(this._ownerId);
      this._ownerId = 0;
    }
    this._dbus?.unexport();
    this._dbus = null;
    this._events = [];
  }

  get ApiVersion() {
    return API_VERSION;
  }

  _connect(obj, signal, handler) {
    this._signals.push([obj, obj.connect(signal, handler)]);
  }

  /** Connect if the signal exists on this GNOME version; skip it if not. */
  _connectOptional(obj, signal, handler) {
    try {
      this._connect(obj, signal, handler);
    } catch (e) {
      console.warn(`gdr-windows: no '${signal}' signal on this shell (${e})`);
    }
  }

  _track(win) {
    if (this._unmanaged.has(win)) return;
    // Snapshot identity now: by the time `unmanaged` fires the window is
    // already torn down and get_title()/get_wm_class() return null, which
    // would make every close event indistinguishable from every other.
    const snapshot = {
      id: win.get_id(),
      title: win.get_title(),
      wm_class: win.get_wm_class(),
      app_id: this._appIdFor(win),
    };
    const handlerId = win.connect("unmanaged", () => {
      const id = this._unmanaged.get(win);
      if (id) {
        try {
          win.disconnect(id);
        } catch {
          /* already disconnected */
        }
      }
      this._unmanaged.delete(win);
      this._pushRaw("closed", snapshot);
    });
    this._unmanaged.set(win, handlerId);
  }

  _appIdFor(win) {
    try {
      const app = Shell.WindowTracker.get_default().get_window_app(win);
      return app ? app.get_id() : null;
    } catch {
      return null;
    }
  }

  _push(kind, win) {
    if (!win) return;
    this._pushRaw(kind, {
      id: win.get_id(),
      title: win.get_title(),
      wm_class: win.get_wm_class(),
      app_id: this._appIdFor(win),
    });
  }

  _pushRaw(kind, ident) {
    this._seq += 1;
    this._events.push({
      seq: this._seq,
      kind,
      // Milliseconds since the epoch: gdrd re-stamps events with its own
      // RFC-3339 clock, this is only here to make the raw bus usable by hand.
      at_ms: Math.round(GLib.get_real_time() / 1000),
      ...ident,
    });
    if (this._events.length > EVENT_RING) {
      this._events.splice(0, this._events.length - EVENT_RING);
    }
    try {
      this._dbus?.emit_signal(
        "Changed",
        new GLib.Variant("(st)", [kind, ident.id ?? 0])
      );
    } catch {
      /* bus went away mid-teardown */
    }
  }

  _describe(win) {
    const frame = rectOf(win.get_frame_rect());
    const workspace = win.get_workspace();
    return {
      id: win.get_id(),
      title: win.get_title(),
      wm_class: win.get_wm_class(),
      wm_class_instance: win.get_wm_class_instance(),
      app_id: this._appIdFor(win),
      gtk_application_id: win.get_gtk_application_id?.() ?? null,
      sandboxed_app_id: win.get_sandboxed_app_id?.() ?? null,
      pid: win.get_pid(),
      window_type: windowTypeName(win.get_window_type()),
      frame_rect: frame,
      buffer_rect: rectOf(win.get_buffer_rect()),
      monitor: win.get_monitor(),
      workspace: workspace ? workspace.index() : null,
      // Compared against the manager rather than read off `workspace.active`:
      // the index comparison is guaranteed API on every version this claims
      // to support, and this flag decides whether a window is screenshottable.
      on_active_workspace: workspace
        ? workspace.index() ===
          global.workspace_manager.get_active_workspace_index()
        : false,
      minimized: win.minimized,
      maximized: maximizedName(maximizeFlagsOf(win)),
      fullscreen: win.is_fullscreen(),
      above: win.is_above(),
      on_all_workspaces: win.is_on_all_workspaces(),
      skip_taskbar: win.is_skip_taskbar(),
      focus: win.has_focus(),
      resizable: win.allows_resize(),
      movable: win.allows_move(),
      can_close: win.can_close(),
    };
  }

  _monitors() {
    const out = [];
    const n = global.display.get_n_monitors();
    const primary = global.display.get_primary_monitor();
    for (let i = 0; i < n; i++) {
      const g = global.display.get_monitor_geometry(i);
      let scale = 1;
      try {
        scale = global.display.get_monitor_scale(i);
      } catch {
        /* very old mutter — gdrd falls back to DisplayConfig */
      }
      out.push({
        index: i,
        x: g.x,
        y: g.y,
        width: g.width,
        height: g.height,
        scale,
        primary: i === primary,
        connector: this._connectorFor(i),
      });
    }
    return out;
  }

  /**
   * DRM connector name ("eDP-1") for a monitor index, or null.
   *
   * This is the join key between the extension's stage coordinates and the
   * connector gdrd chose to record, so gdrd can tell a window on the captured
   * monitor from one on a monitor it is not streaming. Mutter has no direct
   * index→connector getter, so walk the connectors it will answer for.
   */
  _connectorFor(index) {
    try {
      const manager = global.backend.get_monitor_manager();
      if (!manager?.get_monitor_for_connector) return null;
      for (const name of this._knownConnectors()) {
        if (manager.get_monitor_for_connector(name) === index) return name;
      }
    } catch {
      /* fall through to null */
    }
    return null;
  }

  _knownConnectors() {
    // Mutter exposes no connector list to JS. Probing the standard DRM
    // naming space is cheap (a few dozen string lookups, only while
    // listing) and covers every real output; gdrd cross-checks against
    // DisplayConfig anyway, so a miss degrades to null, not to a wrong name.
    if (this._connectorNames) return this._connectorNames;
    const names = [];
    const families = [
      "eDP",
      "LVDS",
      "DP",
      "HDMI",
      "HDMI-A",
      "DVI",
      "DVI-I",
      "DVI-D",
      "VGA",
      "Virtual",
      "Meta",
    ];
    for (const family of families) {
      for (let i = 0; i <= 8; i++) names.push(`${family}-${i}`);
    }
    this._connectorNames = names;
    return names;
  }

  List() {
    const windows = allWindows()
      .filter((w) => w.get_window_type() !== Meta.WindowType.DESKTOP)
      .map((w) => this._describe(w));
    return JSON.stringify({
      api_version: API_VERSION,
      seq: this._seq,
      coordinate_space: "logical",
      focus_window: global.display.focus_window?.get_id() ?? null,
      active_workspace: global.workspace_manager.get_active_workspace_index(),
      n_workspaces: global.workspace_manager.get_n_workspaces(),
      monitors: this._monitors(),
      windows,
    });
  }

  _find(id) {
    return allWindows().find((w) => w.get_id() === id) ?? null;
  }

  Act(id, action, argsJson) {
    const win = this._find(id);
    if (!win) {
      return JSON.stringify({ ok: false, error: `no window with id ${id}` });
    }
    let args = {};
    if (argsJson) {
      try {
        args = JSON.parse(argsJson);
      } catch (e) {
        return JSON.stringify({ ok: false, error: `bad args_json: ${e}` });
      }
    }
    const time = global.get_current_time();
    try {
      switch (action) {
        case "activate":
          // Main.activateWindow is the shell's own path: it unminimizes,
          // switches workspace, and closes the overview. Doing those by hand
          // leaves the overview covering the window we just "activated".
          if (win.minimized) win.unminimize();
          Main.activateWindow(win, time);
          break;
        case "focus":
          win.focus(time);
          break;
        case "raise":
          win.raise();
          break;
        case "minimize":
          win.minimize();
          break;
        case "unminimize":
          win.unminimize();
          break;
        case "maximize":
          win.maximize(Meta.MaximizeFlags.BOTH);
          break;
        case "unmaximize":
          win.unmaximize(Meta.MaximizeFlags.BOTH);
          break;
        case "fullscreen":
          win.make_fullscreen();
          break;
        case "unfullscreen":
          win.unmake_fullscreen();
          break;
        case "above":
          win.make_above();
          break;
        case "unabove":
          win.unmake_above();
          break;
        case "stick":
          win.stick();
          break;
        case "unstick":
          win.unstick();
          break;
        case "close":
          if (!win.can_close()) {
            return JSON.stringify({
              ok: false,
              error: "window refuses programmatic close",
            });
          }
          win.delete(time);
          break;
        case "move": {
          const r = win.get_frame_rect();
          win.move_frame(true, args.x ?? r.x, args.y ?? r.y);
          break;
        }
        case "resize": {
          const r = win.get_frame_rect();
          win.move_resize_frame(
            true,
            r.x,
            r.y,
            args.width ?? r.width,
            args.height ?? r.height
          );
          break;
        }
        case "move_resize": {
          const r = win.get_frame_rect();
          win.move_resize_frame(
            true,
            args.x ?? r.x,
            args.y ?? r.y,
            args.width ?? r.width,
            args.height ?? r.height
          );
          break;
        }
        case "workspace": {
          const index = args.index;
          if (typeof index !== "number") {
            return JSON.stringify({
              ok: false,
              error: "workspace action needs args.index",
            });
          }
          win.change_workspace_by_index(index, false);
          break;
        }
        default:
          return JSON.stringify({ ok: false, error: `unknown action '${action}'` });
      }
    } catch (e) {
      return JSON.stringify({ ok: false, error: String(e) });
    }

    // Re-describe rather than echo the request: the compositor may have
    // clamped a move, refused a resize, or switched workspaces, and the
    // caller needs the state that actually resulted.
    const after = this._find(id);
    return JSON.stringify({
      ok: true,
      action,
      window: after ? this._describe(after) : null,
    });
  }

  Launch(appId) {
    try {
      const system = Shell.AppSystem.get_default();
      const app =
        system.lookup_app(appId) ??
        system.lookup_app(`${appId}.desktop`) ??
        system.lookup_desktop_wmclass(appId);
      if (!app) {
        return JSON.stringify({
          ok: false,
          error: `no installed app matches '${appId}' — call ListApps to see ids`,
        });
      }
      // activate() raises an existing window when the app is already
      // running, and starts it otherwise; that is exactly the "act on a
      // window even if it is not currently open" case.
      const running =
        typeof app.get_n_windows === "function" && app.get_n_windows() > 0;
      app.activate();
      return JSON.stringify({
        ok: true,
        app_id: app.get_id(),
        name: app.get_name(),
        was_running: running,
        action: running ? "activated" : "launched",
      });
    } catch (e) {
      return JSON.stringify({ ok: false, error: String(e) });
    }
  }

  ListApps(filter) {
    const needle = (filter || "").trim().toLowerCase();
    const system = Shell.AppSystem.get_default();

    // get_installed() hands back Gio.AppInfo, NOT Shell.App — it has no
    // get_n_windows(). Window counts come from the running Shell.App set
    // instead, keyed by desktop-file id.
    const windowsById = new Map();
    for (const app of system.get_running()) {
      windowsById.set(app.get_id(), app.get_n_windows());
    }

    const seen = [];
    for (const info of system.get_installed()) {
      // Hidden/NoDisplay entries are not things a user can launch, and they
      // outnumber the real apps badly enough to drown the list.
      if (typeof info.should_show === "function" && !info.should_show()) continue;
      const id = info.get_id();
      if (!id) continue;
      const name = info.get_display_name?.() ?? info.get_name?.() ?? id;
      if (
        needle &&
        !id.toLowerCase().includes(needle) &&
        !name.toLowerCase().includes(needle)
      ) {
        continue;
      }
      const windows = windowsById.get(id) ?? 0;
      seen.push({ app_id: id, name, windows, running: windows > 0 });
    }
    seen.sort((a, b) => a.name.localeCompare(b.name));
    return JSON.stringify({ ok: true, count: seen.length, apps: seen });
  }

  Events(since, limit) {
    const cap = limit > 0 ? Math.min(limit, EVENT_RING) : EVENT_RING;
    const oldest = this._events.length ? this._events[0].seq : this._seq + 1;
    const events = this._events.filter((e) => e.seq > since).slice(0, cap);
    return JSON.stringify({
      ok: true,
      // A caller resuming from a seq older than the ring missed events we
      // no longer have. Say so instead of implying continuity.
      dropped: since > 0 && since + 1 < oldest,
      oldest_seq: oldest,
      next_seq: this._seq,
      events,
    });
  }
}
