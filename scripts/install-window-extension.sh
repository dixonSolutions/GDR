#!/usr/bin/env bash
# Install the gdr-windows GNOME Shell extension on a target.
#
# gdrd can see pixels without this, but not *windows*: GNOME 50 answers
# AccessDenied to org.gnome.Shell.Introspect.GetWindows for everything except
# the desktop portal, and the GSetting that used to open it up is gone. A
# shell extension is the only supported way for a daemon in the session to
# enumerate windows or activate one.
#
# Usage:
#   ./scripts/install-window-extension.sh              # this machine
#   ./scripts/install-window-extension.sh user@host    # over SSH
#   ./scripts/install-window-extension.sh --dev desk   # a configured device
#   ./scripts/install-window-extension.sh --uninstall
#
# IMPORTANT: on Wayland the shell only scans for newly installed extensions
# at session start. After this script the user must log out and back in
# once. `gnome-extensions enable` is not enough and will say the extension
# does not exist, because the running shell has never seen its directory.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
UUID="gdr-windows@gdr.dixonsolutions.github.io"
SRC="$REPO_DIR/shell-extension/$UUID"

TARGET=""
UNINSTALL=0

usage() { sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-0}"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --uninstall) UNINSTALL=1 ;;
    --dev|--host)
      shift
      [ $# -gt 0 ] || { echo "error: --dev needs a device id" >&2; exit 2; }
      TARGET="$(
        python3 - "$1" <<'PY'
import json, os, sys
q = sys.argv[1].strip().lower()
path = os.path.expanduser("~/.config/gdr/config.json")
cfg = json.load(open(path))
for name, p in cfg.get("hosts", {}).items():
    names = [name, p.get("label") or "", *(p.get("aliases") or [])]
    if any(n.strip().lower() == q for n in names if n):
        print(p.get("ssh") or p.get("address") or "")
        break
else:
    sys.exit(f"unknown device '{sys.argv[1]}'")
PY
      )"
      [ -n "$TARGET" ] || { echo "error: device has no ssh/address" >&2; exit 2; }
      ;;
    -h|--help) usage 0 ;;
    *) TARGET="$1" ;;
  esac
  shift
done

# A loopback "target" is this machine; running it through ssh would be a
# pointless (and often unconfigured) round trip.
case "${TARGET,,}" in
  ""|local|localhost|127.0.0.1|::1|loopback|this) TARGET="" ;;
esac

[ -f "$SRC/extension.js" ] || { echo "error: no extension at $SRC" >&2; exit 1; }

remote_script() {
  cat <<'REMOTE'
set -eu
UUID="gdr-windows@gdr.dixonsolutions.github.io"
DEST="$HOME/.local/share/gnome-shell/extensions/$UUID"
if [ "${GDR_UNINSTALL:-0}" = "1" ]; then
  rm -rf "$DEST"
  echo "removed $DEST"
else
  mkdir -p "$DEST"
  cat > "$DEST/metadata.json" <<'META'
__METADATA__
META
  cat > "$DEST/extension.js" <<'EXTJS'
__EXTENSION__
EXTJS
  echo "installed $DEST"
fi

# Enablement lives in dconf, and `gnome-extensions enable` refuses a uuid the
# running shell has not loaded — so write the list directly. It takes effect
# at the next session start, which is exactly when the shell will also find
# the directory.
python3 - "$UUID" "${GDR_UNINSTALL:-0}" <<'PY'
import ast, subprocess, sys
uuid, removing = sys.argv[1], sys.argv[2] == "1"
def get(key):
    out = subprocess.run(["/usr/bin/gsettings", "get", "org.gnome.shell", key],
                         capture_output=True, text=True).stdout.strip()
    if not out or out.startswith("@as"):
        return []
    return ast.literal_eval(out)
def put(key, values):
    literal = "[" + ", ".join("'" + v + "'" for v in values) + "]"
    subprocess.run(["/usr/bin/gsettings", "set", "org.gnome.shell", key, literal], check=True)

enabled = get("enabled-extensions")
if removing:
    if uuid in enabled:
        put("enabled-extensions", [u for u in enabled if u != uuid])
        print("disabled " + uuid)
elif uuid in enabled:
    print("already enabled: " + uuid)
else:
    put("enabled-extensions", enabled + [uuid])
    print("enabled " + uuid)
PY
REMOTE
}

# Fill the template with the real extension sources.
#
# The template goes through a temp file rather than a pipe: `python3 - <<PY`
# already claims stdin for the script itself, so piping into it silently
# yields an empty read and an install that reports success while doing
# nothing.
payload() {
  local tmpl
  tmpl="$(mktemp)"
  remote_script > "$tmpl"
  python3 - "$tmpl" "$SRC/metadata.json" "$SRC/extension.js" <<'PYEOF'
import sys
script = open(sys.argv[1]).read()
meta = open(sys.argv[2]).read().rstrip("\n")
ext = open(sys.argv[3]).read().rstrip("\n")
# Heredoc bodies are quoted ('META'/'EXTJS'), so nothing inside is expanded;
# the only thing that could break out is a line equal to the delimiter.
for body, delim in ((meta, "META"), (ext, "EXTJS")):
    if any(line.strip() == delim for line in body.splitlines()):
        sys.exit(f"refusing to install: source contains a bare {delim} line")
if not script.strip():
    sys.exit("refusing to install: empty install template")
sys.stdout.write(script.replace("__METADATA__", meta).replace("__EXTENSION__", ext))
PYEOF
  rm -f "$tmpl"
}

if [ "$UNINSTALL" = 1 ]; then export GDR_UNINSTALL=1; fi

if [ -z "$TARGET" ]; then
  echo "==> Installing $UUID locally"
  payload | GDR_UNINSTALL="${GDR_UNINSTALL:-0}" bash
  # Verify rather than trust the exit status: a template that came through
  # empty runs cleanly and installs nothing.
  if [ "$UNINSTALL" = 0 ] && [ ! -s "$HOME/.local/share/gnome-shell/extensions/$UUID/extension.js" ]; then
    echo "error: install produced no extension.js — nothing was installed" >&2
    exit 1
  fi
else
  echo "==> Installing $UUID on $TARGET"
  payload | ssh "$TARGET" "GDR_UNINSTALL='${GDR_UNINSTALL:-0}' bash -s"
fi

echo
if [ "$UNINSTALL" = 1 ]; then
  echo "Done. The extension stops running at the next session start."
else
  cat <<'NOTE'
Done — but NOT yet active.

GNOME/Wayland only scans for newly installed extensions when the session
starts, so the running shell cannot load it and `gnome-extensions enable`
will report that it does not exist. Log out and back in once (or reboot).

After logging back in, verify with:

  gdbus call --session -d org.gdr.Windows -o /org/gdr/Windows \
    -m org.gdr.Windows.List | head -c 400

  gdr windows --host <device>
NOTE
fi
