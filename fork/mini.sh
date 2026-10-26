#!/usr/bin/env bash
#
# mini.sh — drive the full-tunnel gate on the dedicated macOS test box.
#
#   fork/mini.sh gate             build + deploy, then two phases, each restart + wait +
#                                 verify: the default config on a dedicated TUN, then the
#                                 same config on a shared virtual NIC plus a second
#                                 instance joining it at runtime (must be refused); ends
#                                 with a stop that checks nothing is left behind (default)
#   fork/mini.sh build [--debug]  cargo build easytier-core/-cli (release by default,
#                                 same arguments easytier-mac/scripts/build-core.sh uses)
#                                 and the fork_instance_ctl example the gate needs
#   fork/mini.sh deploy           rsync binaries + scripts, render config on the box
#   fork/mini.sh start|stop|restart|status|wait|logs [-f]
#                                 (start/restart [--shared], join-conflict: see et-test)
#   fork/mini.sh verify           run fork/verify-fulltunnel.sh on the box; the
#                                 expected egress IP is measured from this machine
#   fork/mini.sh ssh [cmd]        shell on the box
#
# Settings come from fork/mini.env (gitignored); see fork/mini.env.example.
# Why a second machine at all: this one shares the LAN with the exit node, so
# from here the tunnel can never be told apart from the LAN. See fork/SOP.md §4.

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
FORK=fork
ENV_FILE="$FORK/mini.env"
[[ -f "$ENV_FILE" ]] || { echo "missing $ENV_FILE — copy $FORK/mini.env.example and fill it in" >&2; exit 2; }
# shellcheck disable=SC1090
source "$ENV_FILE"

: "${MINI_SSH:=mini}" "${MINI_DIR:=/usr/local/easytier-test}"
: "${ET_MINI_HOSTNAME:=mini}" "${ET_MINI_IPV4:?}" "${ET_EXIT_NODE:?}" "${ET_PEERS:?}"
: "${ET_DEV_CONFIG:?}" "${ET_PING_TARGET:=}" "${ET_PING_MAX_MS:=80}"
: "${ET_EGRESS_CN_URL:=https://ipv4.ddnspod.com}" "${ET_EGRESS_INTL_URL:=https://api.ipify.org}"

PROFILE=release
JQ_STATIC="${JQ_STATIC:-}"   # optional: path to a static jq-macos-arm64 to ship (box has no brew)

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { printf '\033[31m%s\033[0m\n' "$*" >&2; exit 2; }
remote() { ssh -o BatchMode=yes "$MINI_SSH" "$@"; }

# et-test on the box needs the gate parameters; sudo drops the environment,
# so they travel as explicit env assignments on the remote command line
remote_et() {
  local expect="${1:-}"; shift
  remote "ET_EXIT_NODE=$ET_EXIT_NODE ET_PING_TARGET=$ET_PING_TARGET ET_PING_MAX_MS=$ET_PING_MAX_MS" \
         "ET_EXPECT_EGRESS=$expect ET_EGRESS_CN_URL=$ET_EGRESS_CN_URL ET_EGRESS_INTL_URL=$ET_EGRESS_INTL_URL" \
         "$MINI_DIR/bin/et-test" "$@"
}

cmd_build() {
  [[ "${1:-}" == "--debug" ]] && PROFILE=debug
  local args=(build -p easytier --bin easytier-core --bin easytier-cli --example fork_instance_ctl)
  [[ "$PROFILE" == release ]] && args+=(--release)
  say "cargo ${args[*]}"
  cargo "${args[@]}"
}

toml_value() {  # toml_value <key> <file>
  awk -F'"' -v k="$1" '$0 ~ "^[[:space:]]*"k"[[:space:]]*=" {print $2; exit}' "$2"
}

render_config() {
  local name secret peers
  name=$(toml_value network_name "$ET_DEV_CONFIG")
  secret=$(toml_value network_secret "$ET_DEV_CONFIG")
  [[ -n "$name" && -n "$secret" ]] || die "cannot read network_identity from $ET_DEV_CONFIG"
  peers=""
  # awk -v cannot take literal newlines (BSD awk), so pass \n escapes for it to expand
  for uri in $ET_PEERS; do peers+="[[peer]]\\nuri = \"$uri\"\\n\\n"; done
  # placeholders are replaced with awk to keep the secret out of sed's pattern space
  awk -v hostname="$ET_MINI_HOSTNAME" -v ipv4="$ET_MINI_IPV4" -v exit_node="$ET_EXIT_NODE" \
      -v name="$name" -v secret="$secret" -v dir="$MINI_DIR" -v peers="$peers" '
    { gsub(/@HOSTNAME@/, hostname); gsub(/@IPV4@/, ipv4); gsub(/@EXIT_NODE@/, exit_node)
      gsub(/@NETWORK_NAME@/, name); gsub(/@NETWORK_SECRET@/, secret); gsub(/@DIR@/, dir)
      if ($0 == "@PEERS@") { printf "%s", peers } else { print } }' \
    "$FORK/mini/config.toml.tmpl"
}

cmd_deploy() {
  local core="target/$PROFILE/easytier-core" cli="target/$PROFILE/easytier-cli"
  local ctl="target/$PROFILE/examples/fork_instance_ctl"
  [[ -x "$core" && -x "$cli" && -x "$ctl" ]] || die "no $PROFILE binaries — run: $0 build"
  say "preparing $MINI_DIR on $MINI_SSH"
  remote "sudo -n mkdir -p '$MINI_DIR/bin' '$MINI_DIR/logs' '$MINI_DIR/run' && sudo -n chown -R \"\$(id -un)\" '$MINI_DIR'" \
    || die "sudo -n failed on the box — is the NOPASSWD sudoers entry in place?"
  say "shipping binaries ($PROFILE) and scripts"
  local files=("$core" "$cli" "$FORK/et-test.sh" "$FORK/verify-fulltunnel.sh")
  [[ -n "$JQ_STATIC" ]] && files+=("$JQ_STATIC")
  rsync -a --chmod=755 "${files[@]}" "$MINI_SSH:$MINI_DIR/bin/"
  rsync -a --chmod=755 "$ctl" "$MINI_SSH:$MINI_DIR/bin/instance-ctl"
  remote "cd '$MINI_DIR/bin' && mv -f et-test.sh et-test && { [ ! -f jq-macos-arm64 ] || mv -f jq-macos-arm64 jq; }"
  say "rendering config"
  render_config | remote "cat > '$MINI_DIR/config.toml' && chmod 600 '$MINI_DIR/config.toml'"
  remote "command -v jq >/dev/null || test -x '$MINI_DIR/bin/jq'" \
    || die "no jq on the box: set JQ_STATIC=/path/to/jq-macos-arm64 in $ENV_FILE and redeploy"
  remote "'$MINI_DIR/bin/easytier-core' --version && '$MINI_DIR/bin/easytier-cli' --version"
}

expected_egress() {
  # this machine sits behind the same uplink as the exit node, so what the
  # domestic echo shows here is what the box must see through the tunnel
  # retried: the echo service occasionally times out, and the gate asks
  # for it once per verify
  local ip
  for _ in 1 2 3; do
    ip=$(curl -4 -fsS --max-time 8 "$ET_EGRESS_CN_URL" 2>/dev/null | tr -d '[:space:]')
    [[ -n "$ip" ]] && { printf '%s\n' "$ip"; return 0; }
    sleep 2
  done
  return 1
}

cmd_verify() {
  local expect
  expect=$(expected_egress) || die "cannot fetch expected egress from $ET_EGRESS_CN_URL"
  say "expected egress (measured here): $expect"
  remote_et "$expect" verify
}

cmd_gate() {
  cmd_build "$@"
  cmd_deploy
  say "phase 1/2: dedicated TUN — restarting core on the box"
  remote_et "" restart
  say "waiting for the exit node to become p2p"
  remote_et "" wait 90 || true
  cmd_verify
  # since v2.7.1 any dev_name puts the instance on upstream's shared virtual
  # NIC, which tracks route claims per instance; the full tunnel must keep
  # working there and keep upstream's rule that it excludes other members
  say "phase 2/2: shared virtual NIC — restarting core with dev_name set"
  remote_et "" restart --shared
  say "waiting for the exit node to become p2p"
  remote_et "" wait 90 || true
  cmd_verify
  say "a second instance joins the shared NIC at runtime; it must be refused"
  remote_et "" join-conflict
  say "the full-tunnel instance must be unaffected"
  cmd_verify
  say "stopping core; nothing may be left behind"
  remote_et "" stop
}

case "${1:-gate}" in
  gate)    shift; cmd_gate "$@" ;;
  build)   shift; cmd_build "$@" ;;
  deploy)  cmd_deploy ;;
  start|stop|restart|status|logs|wait|join-conflict)
           remote_et "" "$@" ;;
  verify)  cmd_verify ;;
  ssh)     shift; ssh "$MINI_SSH" "$@" ;;
  *)       sed -n '3,23p' "$0"; exit 2 ;;
esac
