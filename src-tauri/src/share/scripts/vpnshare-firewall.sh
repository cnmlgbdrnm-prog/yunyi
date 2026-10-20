#!/bin/sh
# 用法: vpnshare-firewall.sh open|close <comment-prefix> <port:proto>...
set -u
action=$1; prefix=$2; shift 2
for spec in "$@"; do
  port=${spec%%:*}
  proto=${spec##*:}
  [ "$proto" = "$spec" ] && proto=tcp
  comment="$prefix-$port-$proto"
  if [ "$action" = open ]; then
    iptables -C INPUT -p "$proto" --dport "$port" -m comment --comment "$comment" -j ACCEPT 2>/dev/null || \
      iptables -I INPUT 1 -p "$proto" --dport "$port" -m comment --comment "$comment" -j ACCEPT
  else
    while iptables -C INPUT -p "$proto" --dport "$port" -m comment --comment "$comment" -j ACCEPT 2>/dev/null; do
      iptables -D INPUT -p "$proto" --dport "$port" -m comment --comment "$comment" -j ACCEPT
    done
  fi
done
