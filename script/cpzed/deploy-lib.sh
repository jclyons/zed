# script/cpzed/deploy-lib.sh: sourced by deploy, gate, health, which, backup (CPZED-23).
# Paths and every external effect are parameterised by environment so the whole pipeline can
# run against a throwaway prefix with a dummy app (script/cpzed/test-deploy).
#
#   CPZED_HOME_DIR  state, hold, lock, ring/, stock/, backup/, logs/   (default ~/Library/Application Support/CPZed)
#   CPZED_APP_DIR   where CPZed.app lives and where .cpzed-stage/ is   (default /Applications)
#   CPZED_DATA_DIR  Zed user data to back up                           (default ~/Library/Application Support/Zed)
#   CPZED_CONF_DIR  ~/.config/zed                                      (default ~/.config/zed)
#   CPZED_PORT      single-instance port                               (default 43737 + 200 + uid, stable channel)
#   CPZED_QUIT_CMD / CPZED_OPEN_CMD / CPZED_NOTIFY_CMD / CPZED_SIGN_CMD / CPZED_BUNDLE_CMD /
#   CPZED_HID_IDLE_CMD / CPZED_GH_CMD   overrides for the effects (argv-split, no shell)
#   CPZED_TEST_PREFIX  when set, EVERY path above must resolve inside it and the quit, open and
#                      notify effects must be overridden, else the script exits 99 before doing anything.
# Timings are seconds and overridable (CPZED_*_S) so tests run in a blink.

_dl_here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
UTIL=("$_dl_here/deploy-util")
HOME_DIR=${CPZED_HOME_DIR:-$HOME/Library/Application Support/CPZed}
APP_DIR=${CPZED_APP_DIR:-/Applications}
DATA_DIR=${CPZED_DATA_DIR:-$HOME/Library/Application Support/Zed}
CONF_DIR=${CPZED_CONF_DIR:-$HOME/.config/zed}
PORT=${CPZED_PORT:-$((43737 + 200 + $(id -u)))}
STATE=$HOME_DIR/state.json
HOLD=$HOME_DIR/hold
LOCK=$HOME_DIR/deploy.lock
LOGDIR=$HOME_DIR/logs
STAGE_DIR=$APP_DIR/.cpzed-stage
APP=$APP_DIR/CPZed.app
STOCK_APP=$APP_DIR/Zed.app

_dl_rp() { python3 -I -c 'import os,sys;print(os.path.realpath(sys.argv[1]))' "$1"; }

dl_die() { echo "${DL_NAME:-cpzed}: $1" >&2; exit "${2:-1}"; }

# Test-prefix confinement: refuse to run at all if anything points outside the prefix.
dl_confine() {
  [[ -n "${CPZED_TEST_PREFIX:-}" ]] || return 0
  local pre p
  pre=$(_dl_rp "$CPZED_TEST_PREFIX")
  [[ "$pre" != / && "$pre" != /Applications && "$pre" != "$(_dl_rp "$HOME")" || -n "${CPZED_TEST_ALLOW_HOME:-}" ]] \
    || dl_die "test prefix is a protected path: $pre" 99
  for p in "$HOME_DIR" "$APP_DIR" "$DATA_DIR" "$CONF_DIR"; do
    case "$(_dl_rp "$p")/" in "$pre"/*) ;; *) dl_die "TEST CONFINEMENT: $p is outside $pre" 99 ;; esac
  done
  [[ -n "${CPZED_QUIT_CMD:-}" && -n "${CPZED_OPEN_CMD:-}" && -n "${CPZED_NOTIFY_CMD:-}" ]] \
    || dl_die "TEST CONFINEMENT: CPZED_QUIT_CMD, CPZED_OPEN_CMD and CPZED_NOTIFY_CMD must be set under a test prefix (no real osascript/open)" 99
}

dl_init() { dl_confine; mkdir -p "$HOME_DIR" "$LOGDIR"; }

ts() { date +%s; }
log() { local l; l="$(date '+%F %T') ${DL_NAME:-deploy}[$$]: $*"; echo "$l" >&2; [[ -d "$LOGDIR" ]] && echo "$l" >> "$LOGDIR/deploy.log" || true; }

st_get()  { python3 -I "${UTIL[0]}" state "$STATE" get "$1"; }
st_set()  { python3 -I "${UTIL[0]}" state "$STATE" set "$1" "$2"; }
st_setj() { python3 -I "${UTIL[0]}" state "$STATE" setj "$1" "$2"; }
st_del()  { python3 -I "${UTIL[0]}" state "$STATE" del "$1"; }
st_hist() { python3 -I "${UTIL[0]}" state "$STATE" addhist "$1"; }
util()    { python3 -I "${UTIL[0]}" "$@"; }

# split an override command string into an argv without a shell
dl_run() { # varname args...   runs ${!varname} (split) then the args
  local v=$1; shift; local -a c; read -r -a c <<<"${!v}"; "${c[@]}" "$@"
}

# notify <kind> <message>: JSON line in notifications.jsonl (the sre forwards it) + a macOS
# notification (or CPZED_NOTIFY_CMD). A failing notification never fails the deploy.
notify() {
  local kind=$1 msg=$2
  python3 -I - "$HOME_DIR/notifications.jsonl" "$kind" "$msg" <<'PY' || true
import json, sys, time
p, k, m = sys.argv[1:]
with open(p, "a") as f:
    f.write(json.dumps({"at": int(time.time()), "kind": k, "message": m}) + "\n")
PY
  log "NOTIFY[$kind] ${msg%%$'\n'*}"
  if [[ -n "${CPZED_NOTIFY_CMD:-}" ]]; then
    CPZED_NOTIFY_KIND=$kind dl_run CPZED_NOTIFY_CMD "$msg" >/dev/null 2>&1 || true
  else
    /usr/bin/osascript -e 'on run argv' -e 'display notification (item 1 of argv) with title "CPZed deploy"' -e 'end run' "${msg%%$'\n'*}" >/dev/null 2>&1 || true
  fi
}

hold_set() { # reason
  mkdir -p "$HOME_DIR"; printf '%s\n' "$1" > "$HOLD"; log "HOLD set: $1"
}
hold_active() { [[ -e "$HOLD" ]]; }

# pid of whatever listens on the single-instance port (empty when none)
port_pid() { lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null | head -1; }

pid_alive() { [[ -n "${1:-}" ]] && kill -0 "$1" 2>/dev/null; }

app_n()   { python3 -I -c 'import json,sys;print(json.load(open(sys.argv[1])).get("n",""))' "$1/Contents/Resources/cpzed-build.json" 2>/dev/null; }
app_sha() { python3 -I -c 'import json,sys;print(json.load(open(sys.argv[1])).get("sha",""))' "$1/Contents/Resources/cpzed-build.json" 2>/dev/null; }
