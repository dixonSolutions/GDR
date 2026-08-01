#!/usr/bin/env bash
# Unified day-2 updater for gdr.
#
# Default (no host): rebuild package, pick machines (local + SSH remotes from
# config.json), install/restart each, optionally restart Cursor MCP.
#
# Single host: push package (or binary / source build) to one machine and
# restart its gdrd.
#
# Usage:
#   ./scripts/update.sh                         # interactive machine picker
#   ./scripts/update.sh --yes                   # automation: all machines + Cursor
#   ./scripts/update.sh --yes --local-only
#   ./scripts/update.sh --yes --machines local,desktop
#   ./scripts/update.sh --yes --git-pull         # force-pull if worktrees clean
#   ./scripts/update.sh --yes --no-git-pull
#   ./scripts/update.sh --yes --no-restart-cursor
#   ./scripts/update.sh --skip-deps
#   ./scripts/update.sh user@host               # that host only (package, else binary)
#   ./scripts/update.sh user@host --binary
#   ./scripts/update.sh user@host --source
#   GDR_YES=1 GDR_SUDO_PASSWORD=… ./scripts/update.sh
#
# Interactive picker (when not using --yes / --machines):
#   a = yes, all machines
#   s = select specific — number toggles, s lists status, f finishes
#
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/lib/common.sh"

SKIP_DEPS=0
# unset = ask (interactive); 0/1 = forced
DO_REMOTES=""
DO_CURSOR_MCP=""
DO_GIT_PULL=""
HOST_TARGET=""
HOST_MODE="package" # package | binary | source
# Comma-separated machine ids for automation (e.g. local,desktop). Empty = unset.
MACHINES_ARG=""

usage() {
  sed -n '2,28p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-0}"
}

confirm_yes() {
  local reply
  if [ "${GDR_YES:-}" = "1" ]; then return 0; fi
  read -r -p "$1 [Y/n] " reply || true
  [[ ! "${reply:-}" =~ ^[Nn]$ ]]
}

confirm_no() {
  # default-N confirm from common.sh
  confirm "$1"
}

lookup_sudo_for_ssh() {
  local want="$1" cfg id target pass
  cfg="$(controller_config_path)"
  [ -f "$cfg" ] || return 0
  while IFS=$'\t' read -r id target pass; do
    if [ "$target" = "$want" ]; then
      printf '%s' "$pass"
      return 0
    fi
  done < <(list_remote_ssh_targets)
}

while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) usage 0 ;;
    -y|--yes)
      export GDR_YES=1
      # Sensible automation defaults when not overridden later.
      [ -n "$DO_REMOTES" ] || DO_REMOTES=1
      [ -n "$DO_CURSOR_MCP" ] || DO_CURSOR_MCP=1
      [ -n "$DO_GIT_PULL" ] || DO_GIT_PULL=1
      ;;
    --skip-deps) SKIP_DEPS=1 ;;
    --remotes|--all-machines) DO_REMOTES=1; MACHINES_ARG="" ;;
    --no-remotes|--skip-remotes|--local-only)
      DO_REMOTES=0
      MACHINES_ARG="local"
      ;;
    --machines)
      shift
      [ $# -gt 0 ] || die "--machines needs id list (e.g. local,desktop)"
      MACHINES_ARG="$1"
      DO_REMOTES=""
      ;;
    --machines=*)
      MACHINES_ARG="${1#--machines=}"
      DO_REMOTES=""
      ;;
    --git-pull|--pull) DO_GIT_PULL=1 ;;
    --no-git-pull|--no-pull) DO_GIT_PULL=0 ;;
    --restart-cursor|--restart-mcp) DO_CURSOR_MCP=1 ;;
    --no-restart-cursor|--no-restart-mcp) DO_CURSOR_MCP=0 ;;
    --package) HOST_MODE="package" ;;
    --binary) HOST_MODE="binary" ;;
    --source|source) HOST_MODE="source" ;;
    --host)
      shift
      [ $# -gt 0 ] || die "--host needs user@host"
      HOST_TARGET="$1"
      ;;
    -*)
      die "unknown arg: $1 (see --help)"
      ;;
    *@*)
      HOST_TARGET="$1"
      ;;
    *)
      die "unknown arg: $1 (see --help)"
      ;;
  esac
  shift
done

# ---------------------------------------------------------------------------
# Single-host path (legacy update.sh user@host)
# ---------------------------------------------------------------------------
update_one_host() {
  local target="$1" mode="$2"
  local pass pkg

  pass="${GDR_SUDO_PASSWORD:-}"
  if [ -z "$pass" ]; then
    pass="$(lookup_sudo_for_ssh "$target" || true)"
  fi

  echo "gdr update → $target (mode=$mode)"
  echo

  case "$mode" in
    source)
      echo "==> Rsync source + cargo build -p gdrd on $target..."
      ssh_remote "$target" 'mkdir -p ~/gdr-src'
      rsync -az --delete \
        --exclude target --exclude node_modules --exclude dist --exclude .git \
        --exclude docs/data \
        "$ROOT/" "$target:~/gdr-src/"
      ssh_remote "$target" \
        'source "$HOME/.cargo/env" 2>/dev/null; cd ~/gdr-src && cargo build --release -p gdrd'
      ssh_remote "$target" \
        'mkdir -p ~/.local/bin && cp ~/gdr-src/target/release/gdrd ~/.local/bin/gdrd'
      ;;
    binary)
      echo "==> Building gdrd (if needed) and copying binary..."
      if [ ! -x "$ROOT/target/release/gdrd" ]; then
        build_binaries
      fi
      push_remote_binaries "$target" "$pass"
      ;;
    package)
      echo "==> Building local package to install on $target..."
      if [ "$SKIP_DEPS" != "1" ] && [ "${GDR_SKIP_DEPS:-}" != "1" ]; then
        if [ "${GDR_YES:-}" = "1" ] || confirm_no "Refresh apt/dnf build dependencies?"; then
          install_build_deps
        fi
      fi
      build_binaries
      build_mcp
      pkg="$(build_package_file)"
      install_remote_package_file "$target" "$pass" "$pkg"
      ;;
    *)
      die "unknown host mode: $mode"
      ;;
  esac

  echo "==> Restarting gdrd on $target..."
  restart_remote_gdrd "$target"
  ssh_remote "$target" \
    'systemctl --user --no-pager status gdr.service 2>/dev/null | head -20 \
     || systemctl --user --no-pager status gdrd.service 2>/dev/null | head -20 \
     || true' || true
  echo
  echo "Updated $target ($mode)."
}

if [ -n "$HOST_TARGET" ]; then
  update_one_host "$HOST_TARGET" "$HOST_MODE"
  exit 0
fi

# ---------------------------------------------------------------------------
# Machine catalog + interactive / automation selection
# ---------------------------------------------------------------------------
# Parallel arrays (1-based display index = i+1)
MACHINE_KIND=() # local | remote
MACHINE_ID=()
MACHINE_LABEL=()
MACHINE_SSH=()
MACHINE_PASS=()
MACHINE_SEL=() # 0 | 1

load_machines() {
  MACHINE_KIND=("local")
  MACHINE_ID=("local")
  MACHINE_LABEL=("this machine")
  MACHINE_SSH=("")
  MACHINE_PASS=("")
  MACHINE_SEL=(1)

  local id target pass
  while IFS=$'\t' read -r id target pass; do
    [ -n "$target" ] || continue
    MACHINE_KIND+=("remote")
    MACHINE_ID+=("$id")
    MACHINE_LABEL+=("$target")
    MACHINE_SSH+=("$target")
    MACHINE_PASS+=("$pass")
    MACHINE_SEL+=(1)
  done < <(list_remote_ssh_targets)
}

machine_count() {
  echo "${#MACHINE_ID[@]}"
}

print_machine_table() {
  local i n mark kind
  n="$(machine_count)"
  echo "Machines:"
  for ((i = 0; i < n; i++)); do
    if [ "${MACHINE_SEL[$i]}" = 1 ]; then mark="x"; else mark=" "; fi
    kind="${MACHINE_KIND[$i]}"
    printf '  %d) [%s] %-16s  %s\n' \
      "$((i + 1))" "$mark" "${MACHINE_ID[$i]}" "${MACHINE_LABEL[$i]} ($kind)"
  done
  echo "  selected: $(selected_summary)"
}

selected_summary() {
  local i n out=()
  n="$(machine_count)"
  for ((i = 0; i < n; i++)); do
    if [ "${MACHINE_SEL[$i]}" = 1 ]; then
      out+=("${MACHINE_ID[$i]}")
    fi
  done
  if [ "${#out[@]}" -eq 0 ]; then
    echo "(none)"
  else
    local IFS=','
    echo "${out[*]}"
  fi
}

select_all_machines() {
  local i n
  n="$(machine_count)"
  for ((i = 0; i < n; i++)); do MACHINE_SEL[$i]=1; done
}

select_local_only() {
  local i n
  n="$(machine_count)"
  for ((i = 0; i < n; i++)); do
    if [ "${MACHINE_KIND[$i]}" = "local" ]; then MACHINE_SEL[$i]=1; else MACHINE_SEL[$i]=0; fi
  done
}

apply_machines_arg() {
  # MACHINES_ARG = comma/space separated ids (local,desktop) or numbers (1,3)
  local raw="$1" tok i n id
  local -a toks=()
  n="$(machine_count)"
  for ((i = 0; i < n; i++)); do MACHINE_SEL[$i]=0; done

  IFS=',' read -r -a toks <<< "${raw// /}"
  for tok in "${toks[@]}"; do
    [ -n "$tok" ] || continue
    if [[ "$tok" =~ ^[0-9]+$ ]]; then
      i=$((tok - 1))
      if [ "$i" -ge 0 ] && [ "$i" -lt "$n" ]; then
        MACHINE_SEL[$i]=1
      else
        die "unknown machine number: $tok (1–$n)"
      fi
      continue
    fi
    id="$(printf '%s' "$tok" | tr '[:upper:]' '[:lower:]')"
    local found=0
    for ((i = 0; i < n; i++)); do
      if [ "$(printf '%s' "${MACHINE_ID[$i]}" | tr '[:upper:]' '[:lower:]')" = "$id" ]; then
        MACHINE_SEL[$i]=1
        found=1
        break
      fi
    done
    [ "$found" = 1 ] || die "unknown machine id: $tok (have: $(IFS=,; echo "${MACHINE_ID[*]}"))"
  done
}

toggle_machine() {
  local num="$1" i
  i=$((num - 1))
  if [ "$i" -lt 0 ] || [ "$i" -ge "$(machine_count)" ]; then
    echo "  invalid number: $num (1–$(machine_count))" >&2
    return 1
  fi
  if [ "${MACHINE_SEL[$i]}" = 1 ]; then
    MACHINE_SEL[$i]=0
    echo "  deselected ${MACHINE_ID[$i]}"
  else
    MACHINE_SEL[$i]=1
    echo "  selected ${MACHINE_ID[$i]}"
  fi
}

interactive_machine_picker() {
  local reply cmd
  echo "Update machines?"
  echo "  a / y)  yes — all machines"
  echo "  s)      select specific"
  read -r -p "> " reply || true
  reply="$(printf '%s' "${reply:-}" | tr '[:upper:]' '[:lower:]')"
  case "$reply" in
    ""|a|y|yes)
      select_all_machines
      echo "==> All machines selected: $(selected_summary)"
      return 0
      ;;
    s|select)
      ;;
    *)
      echo "Unrecognized — entering select mode."
      ;;
  esac

  echo
  echo "Select machines — type number to toggle, s = status, f = finish"
  print_machine_table
  while true; do
    read -r -p "> " cmd || true
    cmd="$(printf '%s' "${cmd:-}" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]')"
    case "$cmd" in
      f|finish|done)
        if [ "$(selected_summary)" = "(none)" ]; then
          echo "  nothing selected — pick at least one, or a for all" >&2
          continue
        fi
        echo "==> Continuing with: $(selected_summary)"
        return 0
        ;;
      "")
        echo "  type a number to toggle, s for status, f to finish" >&2
        ;;
      s|status|l|list)
        print_machine_table
        ;;
      a|all)
        select_all_machines
        print_machine_table
        ;;
      n|none)
        local i n
        n="$(machine_count)"
        for ((i = 0; i < n; i++)); do MACHINE_SEL[$i]=0; done
        print_machine_table
        ;;
      *[0-9]*)
        if [[ "$cmd" =~ ^[0-9]+$ ]]; then
          toggle_machine "$cmd" || true
        else
          echo "  type a number, s, or f" >&2
        fi
        ;;
      *)
        echo "  commands: <n> toggle · s status · a all · n none · f finish" >&2
        ;;
    esac
  done
}

resolve_machine_selection() {
  load_machines

  if [ -n "$MACHINES_ARG" ]; then
    apply_machines_arg "$MACHINES_ARG"
    echo "==> Machines (--machines): $(selected_summary)"
    return 0
  fi

  if [ "${DO_REMOTES:-}" = "0" ]; then
    select_local_only
    echo "==> Machines (local-only): $(selected_summary)"
    return 0
  fi

  if [ "${DO_REMOTES:-}" = "1" ] || [ "${GDR_YES:-}" = "1" ]; then
    select_all_machines
    echo "==> Machines (all): $(selected_summary)"
    return 0
  fi

  # Interactive
  if [ "$(machine_count)" -eq 1 ]; then
    select_all_machines
    echo "==> Only local machine in catalog — selected"
    return 0
  fi
  interactive_machine_picker
}

local_selected() {
  local i n
  n="$(machine_count)"
  for ((i = 0; i < n; i++)); do
    if [ "${MACHINE_KIND[$i]}" = "local" ] && [ "${MACHINE_SEL[$i]}" = 1 ]; then
      return 0
    fi
  done
  return 1
}

selected_remote_ids() {
  local i n
  n="$(machine_count)"
  for ((i = 0; i < n; i++)); do
    if [ "${MACHINE_KIND[$i]}" = "remote" ] && [ "${MACHINE_SEL[$i]}" = 1 ]; then
      printf '%s\n' "${MACHINE_ID[$i]}"
    fi
  done
}

# Force-pull latest on every selected machine (local + remotes).
# Skips any worktree with local changes (never destroys dirty state).
git_pull_selected_machines() {
  local i n fail=0 dir
  n="$(machine_count)"
  echo "==> Git force-pull on selected machines (only if clean)..."
  for ((i = 0; i < n; i++)); do
    [ "${MACHINE_SEL[$i]}" = 1 ] || continue
    if [ "${MACHINE_KIND[$i]}" = "local" ]; then
      dir="$(find_gdr_git_dir || true)"
      if [ -z "$dir" ]; then
        echo "    skip local: no GDR git checkout found"
        continue
      fi
      git_force_pull_if_clean "$dir" "local/${MACHINE_ID[$i]}" || fail=$((fail + 1))
    else
      remote_git_force_pull_if_clean "${MACHINE_SSH[$i]}" "${MACHINE_ID[$i]}" \
        || fail=$((fail + 1))
    fi
  done
  [ "$fail" -eq 0 ]
}

# ---------------------------------------------------------------------------
# Full update: local package (+ remotes + Cursor)
# ---------------------------------------------------------------------------
echo "gdr update (version $VERSION) — from current tree"
echo

resolve_machine_selection

if [ -z "$DO_GIT_PULL" ]; then
  if confirm_yes "Git force-pull latest on selected machines (skips dirty worktrees)?"; then
    DO_GIT_PULL=1
  else
    DO_GIT_PULL=0
  fi
fi

if [ -z "$DO_CURSOR_MCP" ]; then
  if confirm_yes "Restart Cursor gdr MCP after install?"; then
    DO_CURSOR_MCP=1
  else
    DO_CURSOR_MCP=0
  fi
fi

if [ "$DO_GIT_PULL" = 1 ]; then
  if ! git_pull_selected_machines; then
    echo "warning: one or more git pulls failed — continuing" >&2
  fi
else
  echo "==> Skipping git pull"
fi

if [ "$SKIP_DEPS" != "1" ] && [ "${GDR_SKIP_DEPS:-}" != "1" ]; then
  if [ "${GDR_YES:-}" = "1" ] || confirm_no "Refresh apt/dnf build dependencies?"; then
    install_build_deps
  fi
fi

build_binaries
build_mcp

pkg="$(build_package_file)"

remote_ok=1
DO_LOCAL=0
if local_selected; then DO_LOCAL=1; fi

if [ "$DO_LOCAL" = 1 ]; then
  echo "==> Installing local package: $pkg"
  install_package_file "$pkg"
  echo "Installed system package: $pkg"

  if systemctl --user is-active --quiet gdr.service 2>/dev/null \
    || systemctl --user is-active --quiet gdrd.service 2>/dev/null; then
    echo "==> Restarting local gdr user service..."
    systemctl --user restart gdr.service 2>/dev/null \
      || systemctl --user restart gdrd.service 2>/dev/null \
      || true
    systemctl --user --no-pager status gdr.service 2>/dev/null | head -15 \
      || systemctl --user --no-pager status gdrd.service 2>/dev/null | head -15 \
      || true
  fi
else
  echo "==> Skipping local package install (not selected)"
fi

mapfile -t REMOTE_IDS < <(selected_remote_ids)
if [ "${#REMOTE_IDS[@]}" -gt 0 ]; then
  if ! update_remote_packages "$pkg" --only "${REMOTE_IDS[@]}"; then
    remote_ok=0
    echo "warning: one or more remotes failed — see above" >&2
  fi
else
  echo "==> Skipping remotes (none selected)"
fi

if [ "$DO_CURSOR_MCP" = 1 ]; then
  restart_cursor_mcp
else
  echo "==> Skipping Cursor MCP restart"
fi

echo
echo "Update complete."
echo "  Package:  $pkg"
echo "  Machines: $(selected_summary)"
if [ "$DO_GIT_PULL" = 1 ]; then
  echo "  Git:      force-pull attempted (dirty worktrees skipped)"
fi
if [ "$DO_CURSOR_MCP" = 1 ]; then
  echo "  Cursor:   gdr MCP processes restarted"
fi

[ "$remote_ok" = 1 ]
