#!/usr/bin/env bash
#
# et-test — runs ON the test box. Manages a bare easytier-core process for the
# full-tunnel gate. Installed by fork/mini.sh into $MINI_DIR/bin/et-test.
#
#   et-test start          record underlay egress IP, start core, wait for rpc
#   et-test stop           stop core, report leftover split routes
#   et-test restart
#   et-test status         node / peer / route as seen by easytier-cli
#   et-test wait [secs]    block until the exit node is p2p (default 90s)
#   et-test verify         run verify-fulltunnel.sh against the running core
#   et-test logs [-f]
#
# Everything that touches the TUN or root-owned sockets re-execs itself under
# sudo -n; the box has a NOPASSWD sudoers entry for that.
#
# Environment (passed through by fork/mini.sh, all optional):
#   ET_EXIT_NODE ET_PING_TARGET ET_PING_MAX_MS ET_EXPECT_EGRESS
#   ET_EGRESS_CN_URL ET_EGRESS_INTL_URL

set -uo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$DIR/bin"
CORE="$BIN/easytier-core"
CLI="$BIN/easytier-cli"
CONFIG="$DIR/config.toml"
LOGS="$DIR/logs"
RUN="$DIR/run"
RPC="127.0.0.1:15888"
ET_EGRESS_CN_URL="${ET_EGRESS_CN_URL:-https://ipv4.ddnspod.com}"

export PATH="$BIN:$PATH"
ARGS=("$@")

die() { printf '\033[31met-test: %s\033[0m\n' "$*" >&2; exit 2; }
log() { printf 'et-test: %s\n' "$*"; }

need_root() {
  [[ $EUID -eq 0 ]] && return
  exec sudo -n env \
    ET_EXIT_NODE="${ET_EXIT_NODE:-}" ET_PING_TARGET="${ET_PING_TARGET:-}" \
    ET_PING_MAX_MS="${ET_PING_MAX_MS:-}" ET_EXPECT_EGRESS="${ET_EXPECT_EGRESS:-}" \
    ET_EGRESS_CN_URL="$ET_EGRESS_CN_URL" ET_EGRESS_INTL_URL="${ET_EGRESS_INTL_URL:-}" \
    "$0" "${ARGS[@]}"
}

core_pid() { pgrep -f "$CORE" | head -1; }
cli() { "$CLI" -p "$RPC" "$@"; }

leftover_routes() {
  # split routes are the patch's footprint; after stop none may remain
  netstat -rn -f inet | awk '$4 ~ /^utun/ && $1 ~ /^(1|2|4|8|16|32|64|128)(\/[0-9]+)?$/'
}

static_host_routes() {
  # bypass routes are static /32s via the physical gateway
  # (flags UGHS, or UGS on an "a.b.c.d/32" destination); they outlive the TUN,
  # so only a clean shutdown removes them
  netstat -rn -f inet |
    awk '$3 ~ /G/ && $3 ~ /S/ && ($3 ~ /H/ || $1 ~ /\/32$/) {print $1, $2, $4}' | sort
}

cmd_start() {
  need_root
  [[ -x "$CORE" ]] || die "$CORE missing — run fork/mini.sh deploy"
  [[ -f "$CONFIG" ]] || die "$CONFIG missing — run fork/mini.sh deploy"
  if [[ -n "$(core_pid)" ]]; then die "easytier-core already running (pid $(core_pid))"; fi
  mkdir -p "$LOGS" "$RUN"

  # the underlay egress IP must be taken BEFORE the tunnel is up: afterwards
  # every unbound request goes through the exit node
  underlay=$(curl -4 -fsS --max-time 8 "$ET_EGRESS_CN_URL" 2>/dev/null | tr -d '[:space:]')
  if [[ -n "$underlay" ]]; then
    printf '%s\n' "$underlay" > "$RUN/underlay_ip"
    log "underlay egress IP: $underlay"
  else
    rm -f "$RUN/underlay_ip"
    log "WARN: could not fetch underlay egress IP from $ET_EGRESS_CN_URL"
  fi

  static_host_routes > "$RUN/host_routes.before"

  printf '\n===== %s start =====\n' "$(date '+%F %T')" >> "$LOGS/core.log"
  nohup "$CORE" -c "$CONFIG" --rpc-portal "$RPC" >> "$LOGS/core.log" 2>&1 &
  echo $! > "$RUN/core.pid"
  log "started easytier-core pid $! (log: $LOGS/core.log)"

  for _ in $(seq 1 30); do
    if cli -o json node >/dev/null 2>&1; then
      log "rpc portal up at $RPC"
      return 0
    fi
    [[ -z "$(core_pid)" ]] && { tail -20 "$LOGS/core.log"; die "easytier-core exited early"; }
    sleep 1
  done
  die "rpc portal not reachable after 30s"
}

cmd_stop() {
  need_root
  pid=$(core_pid)
  if [[ -z "$pid" ]]; then
    log "easytier-core not running"
  else
    kill "$pid" 2>/dev/null
    for _ in $(seq 1 10); do
      [[ -z "$(core_pid)" ]] && break
      sleep 1
    done
    if [[ -n "$(core_pid)" ]]; then
      log "WARN: still alive after 10s, sending KILL"
      kill -9 "$(core_pid)" 2>/dev/null
      sleep 1
    fi
    log "stopped easytier-core (was pid $pid)"
  fi
  rm -f "$RUN/core.pid"
  rc=0
  left=$(leftover_routes)
  if [[ -n "$left" ]]; then
    printf '\033[33met-test: WARN split routes left behind on a utun:\033[0m\n%s\n' "$left"
    rc=1
  fi
  if [[ -f "$RUN/host_routes.before" ]]; then
    leaked=$(comm -13 "$RUN/host_routes.before" <(static_host_routes))
    if [[ -n "$leaked" ]]; then
      printf '\033[33met-test: WARN bypass host routes left behind:\033[0m\n%s\n' "$leaked"
      rc=1
    fi
  fi
  return $rc
}

cmd_status() {
  pid=$(core_pid)
  if [[ -z "$pid" ]]; then echo "easytier-core: not running"; return 1; fi
  echo "easytier-core: pid $pid, rpc $RPC"
  [[ -f "$RUN/underlay_ip" ]] && echo "underlay egress IP: $(cat "$RUN/underlay_ip")"
  echo; cli node 2>&1 | head -40
  echo; cli peer 2>&1
  echo; cli route 2>&1
}

cmd_wait() {
  secs="${1:-90}"
  [[ -n "${ET_EXIT_NODE:-}" ]] || die "ET_EXIT_NODE not set"
  [[ -n "$(core_pid)" ]] || die "easytier-core not running"
  for i in $(seq 1 "$secs"); do
    peers=$(cli -o json peer 2>/dev/null |
      jq -c 'if type == "array" then . else (.value // .[0].value // []) end' 2>/dev/null)
    row=$(jq -c --arg ip "$ET_EXIT_NODE" '.[] | select((.ipv4 | split("/")[0]) == $ip)' <<<"$peers" 2>/dev/null)
    if [[ -n "$row" ]]; then
      cost=$(jq -r '.cost' <<<"$row"); proto=$(jq -r '.tunnel_proto' <<<"$row")
      if [[ "$cost" == "p2p" ]]; then
        log "exit node $ET_EXIT_NODE is p2p over $proto after ${i}s"
        return 0
      fi
      [[ $((i % 10)) -eq 0 ]] && log "${i}s: exit node is $cost/$proto, waiting for hole punch"
    else
      [[ $((i % 10)) -eq 0 ]] && log "${i}s: exit node not in peer list yet"
    fi
    sleep 1
  done
  log "exit node not p2p after ${secs}s"
  cli peer 2>&1
  return 1
}

cmd_verify() {
  need_root
  [[ -n "$(core_pid)" ]] || die "easytier-core not running"
  underlay=""
  [[ -f "$RUN/underlay_ip" ]] && underlay=$(cat "$RUN/underlay_ip")
  ET_CLI="$CLI" ET_RPC="$RPC" ET_UNDERLAY_IP="$underlay" \
    ET_EXIT_NODE="${ET_EXIT_NODE:-}" ET_PING_TARGET="${ET_PING_TARGET:-}" \
    ET_PING_MAX_MS="${ET_PING_MAX_MS:-80}" ET_EXPECT_EGRESS="${ET_EXPECT_EGRESS:-}" \
    ET_EGRESS_CN_URL="$ET_EGRESS_CN_URL" ET_EGRESS_INTL_URL="${ET_EGRESS_INTL_URL:-}" \
    bash "$BIN/verify-fulltunnel.sh"
}

cmd_logs() {
  if [[ "${1:-}" == "-f" ]]; then tail -f "$LOGS/core.log"; else tail -100 "$LOGS/core.log"; fi
}

case "${1:-}" in
  start)   shift; cmd_start "$@" ;;
  stop)    shift; cmd_stop "$@" ;;
  restart) shift; cmd_stop; cmd_start ;;
  status)  cmd_status ;;
  wait)    shift; cmd_wait "$@" ;;
  verify)  shift; cmd_verify "$@" ;;
  logs)    shift; cmd_logs "$@" ;;
  *) sed -n '3,20p' "$0"; exit 2 ;;
esac
