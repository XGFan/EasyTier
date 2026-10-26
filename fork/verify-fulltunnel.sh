#!/usr/bin/env bash
# Live smoke check for the macOS full tunnel.
#
# Unit tests cannot reach any of this: it is routing table state, socket
# scoping and real egress. Run it against a running easytier-core after every
# rebase onto a new upstream release, before pinning the build anywhere.
#
#   fork/verify-fulltunnel.sh
#
# Configuration (env or a fork/verify-fulltunnel.env file next to this script):
#   ET_CLI            path to easytier-cli           (default: search PATH + app bundle)
#   ET_RPC            rpc portal host:port           (default: autodetect, then 127.0.0.1:15888)
#   ET_EXIT_NODE      exit node virtual IP           e.g. 10.126.126.2   (required for P2P check)
#   ET_PING_TARGET    host behind the exit node      e.g. 192.168.2.231  (optional)
#   ET_PING_MAX_MS    fail above this minimum RTT    (default: 80)
#   ET_EXPECT_EGRESS  exit node's public IP          (optional; else egress is only reported)
#   ET_UNDERLAY_IP    this box's own public IP before the tunnel came up
#                     (optional; et-test start records it; sharpens checks 6 and 7b)
#   ET_EGRESS_CN_URL  domestic IPv4 echo             (default: https://ipv4.ddnspod.com)
#   ET_EGRESS_INTL_URL foreign IPv4 echo             (default: https://api.ipify.org)
#
# The echo services are split on purpose: from behind a Chinese uplink the
# foreign one is unreachable without the tunnel, and the home router proxies
# foreign traffic, so only the domestic echo returns the exit node's real IP.
#
# Exit code 0 only when every check passed.

set -uo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1
[[ -f fork/verify-fulltunnel.env ]] && source fork/verify-fulltunnel.env

ET_CLI="${ET_CLI:-}"
ET_RPC="${ET_RPC:-}"
ET_EXIT_NODE="${ET_EXIT_NODE:-}"
ET_PING_TARGET="${ET_PING_TARGET:-}"
ET_PING_MAX_MS="${ET_PING_MAX_MS:-80}"
ET_EXPECT_EGRESS="${ET_EXPECT_EGRESS:-}"
ET_UNDERLAY_IP="${ET_UNDERLAY_IP:-}"
ET_EGRESS_CN_URL="${ET_EGRESS_CN_URL:-https://ipv4.ddnspod.com}"
ET_EGRESS_INTL_URL="${ET_EGRESS_INTL_URL:-https://api.ipify.org}"

pass=0 fail=0 skip=0
ok()   { printf '  \033[32mPASS\033[0m  %s\n' "$*"; pass=$((pass + 1)); }
no()   { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; fail=$((fail + 1)); }
sk()   { printf '  \033[33mSKIP\033[0m  %s\n' "$*"; skip=$((skip + 1)); }
info() { printf '        %s\n' "$*"; }
step() { printf '\n\033[1m%s\033[0m\n' "$*"; }

die() { printf '\033[31m%s\033[0m\n' "$*" >&2; exit 2; }

command -v jq >/dev/null || die "jq is required (brew install jq)"

# ---------------------------------------------------------------- discovery

step "0. easytier-core"

core_pid=$(pgrep -f 'easytier-core' | head -1)
if [[ -z "$core_pid" ]]; then
  die "easytier-core is not running — start the tunnel first"
fi
ok "easytier-core running (pid $core_pid)"

if [[ -z "$ET_CLI" ]]; then
  ET_CLI=$(command -v easytier-cli)
fi
if [[ -z "$ET_CLI" && -x "/Library/Application Support/EasyTier/bin/easytier-cli" ]]; then
  ET_CLI="/Library/Application Support/EasyTier/bin/easytier-cli"
fi
[[ -x "$ET_CLI" ]] || die "easytier-cli not found; set ET_CLI"

# the supervisor gives core a random rpc portal, so read it off the process
if [[ -z "$ET_RPC" ]]; then
  ET_RPC=$(lsof -nP -a -p "$core_pid" -iTCP -sTCP:LISTEN 2>/dev/null |
    awk 'NR>1 {print $9}' | grep -E '^(127\.0\.0\.1|\*):' | head -1 |
    sed 's/^\*/127.0.0.1/')
fi
ET_RPC="${ET_RPC:-127.0.0.1:15888}"

cli() { "$ET_CLI" -p "$ET_RPC" "$@"; }

node_json=$(cli -o json node 2>/dev/null)
if ! jq -e . >/dev/null 2>&1 <<<"$node_json"; then
  die "cannot talk to easytier-core rpc portal at $ET_RPC (set ET_RPC)"
fi
# a single instance prints the InstanceResult directly, several print a wrapper
node=$(jq -c 'if has("value") then .value else . end' <<<"$node_json")
ok "rpc portal reachable at $ET_RPC"

tun_ip=$(jq -r '.ipv4_addr // ""' <<<"$node" | cut -d/ -f1)
[[ -n "$tun_ip" ]] || die "core reports no virtual IPv4; is the tunnel up?"
tun_if=$(ifconfig 2>/dev/null | awk -v ip="$tun_ip" '
  /^[a-z0-9]+:/ { iface = substr($1, 1, length($1) - 1) }
  $1 == "inet" && $2 == ip { print iface; exit }')
[[ -n "$tun_if" ]] || die "no interface carries the virtual IP $tun_ip"
info "TUN $tun_if / $tun_ip, rpc $ET_RPC"

phys_line=$(netstat -rn -f inet | awk '$1 == "default" && $4 !~ /^(utun|tun|tap|feth|bridge|lo)/ {print; exit}')
phys_gw=$(awk '{print $2}' <<<"$phys_line")
phys_if=$(awk '{print $4}' <<<"$phys_line")
[[ -n "$phys_gw" ]] || die "no physical default route — cannot verify underlay bypass"
info "physical default via $phys_gw on $phys_if"

# ------------------------------------------------------------ split routes

step "1. split default routes on the TUN"

SPLIT_ROUTES=("1.0.0.0 255.0.0.0" "2.0.0.0 254.0.0.0" "4.0.0.0 252.0.0.0" \
              "8.0.0.0 248.0.0.0" "16.0.0.0 240.0.0.0" "32.0.0.0 224.0.0.0" \
              "64.0.0.0 192.0.0.0" "128.0.0.0 128.0.0.0")

missing=()
for entry in "${SPLIT_ROUTES[@]}"; do
  read -r dst mask <<<"$entry"
  got=$(route -n get "$dst" -netmask "$mask" 2>/dev/null |
    awk -F': *' '/^ *(destination|interface)/ {printf "%s ", $2}')
  read -r got_dst got_if <<<"$got"
  if [[ "$got_dst" != "$dst" || "$got_if" != "$tun_if" ]]; then
    missing+=("$dst/$mask (got dst='$got_dst' if='$got_if')")
  fi
done
if [[ ${#missing[@]} -eq 0 ]]; then
  ok "all 8 split routes present on $tun_if"
else
  no "split route set incomplete — part of the address space is not tunnelled"
  printf '        %s\n' "${missing[@]}"
fi

step "2. no route targets 0.0.0.0 on the TUN"
# such routes get RTF_GLOBAL from the kernel, which makes sendto() from
# IP_BOUND_IF-scoped underlay sockets fail with EHOSTUNREACH
bad_default=$(netstat -rn -f inet | awk -v tun="$tun_if" '$1 == "default" && $4 == tun')
if [[ -z "$bad_default" ]]; then
  ok "no 0.0.0.0-destination route on $tun_if"
else
  no "a default route sits on the TUN; scoped underlay sockets will fail"
  printf '        %s\n' "$bad_default"
fi

# ---------------------------------------------------------- underlay escape

step "3. underlay sockets stay off the TUN"

# column 9 is the address pair, column 10 the TCP state (empty for UDP)
underlay=$(lsof -nP -a -p "$core_pid" -i 2>/dev/null | awk 'NR>1 {print $9, $10}')
local_on_tun=$(grep -c "^${tun_ip}:" <<<"$underlay")
if [[ "$local_on_tun" -eq 0 ]]; then
  ok "no easytier socket is sourced from the TUN address"
else
  no "$local_on_tun socket(s) sourced from $tun_ip — underlay traffic is looping into the tunnel"
fi

step "4. bypass /32 routes for live underlay endpoints"

# only live connections carry a bypass route; a connector probing a peer's
# advertised LAN address sits in SYN_SENT for a moment (scoped to the physical
# interface, it cannot get anywhere) and is not a routing failure
remotes=$(grep -v '(SYN_SENT)' <<<"$underlay" |
  grep -oE '\->[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' | cut -c3- | sort -u)
if [[ -z "$remotes" ]]; then
  sk "no established underlay connections to check"
else
  unprotected=()
  for ip in $remotes; do
    [[ "$ip" == 127.* ]] && continue
    read -r r_dst r_gw r_if < <(route -n get "$ip" 2>/dev/null |
      awk -F': *' '/^ *(destination|gateway|interface)/ {printf "%s ", $2} END {print ""}')
    # fine either way: a /32 via the physical gateway, or on-link on a
    # physical interface (no bypass needed there)
    if [[ "$r_dst" == "$ip" && "$r_gw" == "$phys_gw" ]] || [[ "$r_if" == "$phys_if" ]]; then
      continue
    fi
    unprotected+=("$ip -> dst=$r_dst gw=$r_gw if=$r_if")
  done
  if [[ ${#unprotected[@]} -eq 0 ]]; then
    ok "every underlay endpoint resolves onto the physical path"
  else
    no "underlay endpoints routed into the tunnel:"
    printf '        %s\n' "${unprotected[@]}"
  fi
fi

# ------------------------------------------------------------- peer quality

step "5. exit node is reached peer-to-peer"

peer_json=$(cli -o json peer 2>/dev/null)
peers=$(jq -c 'if type == "array" then . else (.value // .[0].value // []) end' <<<"$peer_json")
if [[ -z "$ET_EXIT_NODE" ]]; then
  sk "ET_EXIT_NODE not set"
  jq -r '.[] | "        \(.ipv4)\t\(.cost)\t\(.tunnel_proto)\t\(.lat_ms)ms"' <<<"$peers" 2>/dev/null
else
  row=$(jq -c --arg ip "$ET_EXIT_NODE" '.[] | select((.ipv4 | split("/")[0]) == $ip)' <<<"$peers")
  if [[ -z "$row" ]]; then
    no "exit node $ET_EXIT_NODE not in the peer list"
  else
    cost=$(jq -r '.cost' <<<"$row")
    proto=$(jq -r '.tunnel_proto' <<<"$row")
    lat=$(jq -r '.lat_ms' <<<"$row")
    loss=$(jq -r '.loss_rate' <<<"$row")
    if [[ "$cost" == "p2p" ]]; then
      ok "exit node is p2p over $proto (${lat}ms, loss $loss)"
    else
      no "exit node is '$cost' over $proto (${lat}ms) — hole punching failed, traffic is relayed"
    fi
  fi
fi

step "6. STUN did not report the exit node's address"
# under full tunnel an unscoped STUN probe comes back with the exit node's
# public IP, which then gets announced to peers and breaks hole punching
stun_ip=$(jq -r '(.stun_info.public_ip // [])[]' <<<"$node" 2>/dev/null |
  grep -E '^[0-9]+\.' | head -1)
egress_ip=$(curl -4 -fsS --max-time 8 "$ET_EGRESS_CN_URL" 2>/dev/null | tr -d '[:space:]')
if [[ -z "$stun_ip" ]]; then
  sk "core reports no public IPv4 from STUN yet"
elif [[ -n "$ET_UNDERLAY_IP" && "$stun_ip" != "$ET_UNDERLAY_IP" ]]; then
  no "STUN reports $stun_ip but the underlay's own public IP is $ET_UNDERLAY_IP: the probe did not go out the physical path"
elif [[ -z "$egress_ip" ]]; then
  sk "could not determine egress IP (no internet?)"
elif [[ "$stun_ip" == "$egress_ip" ]]; then
  no "STUN public IP == egress IP ($stun_ip): the STUN probe went through the tunnel"
else
  ok "STUN reports $stun_ip, tunnel egress is $egress_ip"
fi

step "7a. domestic traffic exits through the exit node"
if [[ -z "$egress_ip" ]]; then
  sk "no egress IP from $ET_EGRESS_CN_URL"
elif [[ -z "$ET_EXPECT_EGRESS" ]]; then
  sk "ET_EXPECT_EGRESS not set — egress is $egress_ip, confirm it is the exit node's"
elif [[ "$egress_ip" == "$ET_EXPECT_EGRESS" ]]; then
  ok "egress IP is the exit node ($egress_ip)"
else
  no "egress IP is $egress_ip, expected $ET_EXPECT_EGRESS — full tunnel is not in effect"
fi

step "7b. foreign traffic exits through the exit node"
# the exit node's side proxies foreign traffic, so the IP is not predictable;
# what matters is that the request succeeds at all (it cannot from the bare
# uplink) and that it is not this box's own underlay IP
intl_ip=$(curl -4 -fsS --max-time 10 "$ET_EGRESS_INTL_URL" 2>/dev/null | tr -d '[:space:]')
if [[ -z "$intl_ip" ]]; then
  no "$ET_EGRESS_INTL_URL unreachable — foreign traffic is not going through the tunnel"
elif [[ -n "$ET_UNDERLAY_IP" && "$intl_ip" == "$ET_UNDERLAY_IP" ]]; then
  no "foreign egress is this box's own underlay IP ($intl_ip) — traffic bypassed the tunnel"
else
  ok "foreign egress is $intl_ip (underlay: ${ET_UNDERLAY_IP:-unknown})"
fi

step "8. latency through the tunnel"
if [[ -z "$ET_PING_TARGET" ]]; then
  sk "ET_PING_TARGET not set"
else
  # judged on the minimum RTT: it tells a direct path from a relayed one just
  # as well as the average, but does not flap on a jittery uplink (cellular)
  stats=$(ping -c 10 -i 0.5 -t 15 "$ET_PING_TARGET" 2>/dev/null |
    awk -F'[/ ]+' '/round-trip|rtt/ {printf "%.0f %.0f %.0f", $(NF-4), $(NF-3), $(NF-2)}')
  read -r rtt_min rtt_avg rtt_max <<<"$stats"
  if [[ -z "$rtt_min" ]]; then
    no "$ET_PING_TARGET unreachable"
  elif [[ "$rtt_min" -le "$ET_PING_MAX_MS" ]]; then
    ok "$ET_PING_TARGET min ${rtt_min}ms (<= ${ET_PING_MAX_MS}ms; avg ${rtt_avg}, max ${rtt_max})"
  else
    no "$ET_PING_TARGET min ${rtt_min}ms (> ${ET_PING_MAX_MS}ms; avg ${rtt_avg}, max ${rtt_max}) — likely relayed, not direct"
  fi
fi

printf '\n\033[1m%d passed, %d failed, %d skipped\033[0m\n' "$pass" "$fail" "$skip"
[[ "$fail" -eq 0 ]]
