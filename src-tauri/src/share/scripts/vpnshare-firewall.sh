#!/bin/sh
# 用法: vpnshare-firewall.sh open|close <comment-prefix> <port:proto>...
#
# 同时维护 IPv4 与 IPv6：分享节点可能用 IPv6 地址（服务器 addr_family=v6），
# 只写 iptables 时 UFW 的 ip6tables 默认 DROP 会把数据端口全部挡掉。
set -u
action=$1; prefix=$2; shift 2

apply_one() {
  tool=$1; port=$2; proto=$3; comment=$4
  if [ "$action" = open ]; then
    "$tool" -C INPUT -p "$proto" --dport "$port" -m comment --comment "$comment" -j ACCEPT 2>/dev/null || \
      "$tool" -I INPUT 1 -p "$proto" --dport "$port" -m comment --comment "$comment" -j ACCEPT
  else
    while "$tool" -C INPUT -p "$proto" --dport "$port" -m comment --comment "$comment" -j ACCEPT 2>/dev/null; do
      "$tool" -D INPUT -p "$proto" --dport "$port" -m comment --comment "$comment" -j ACCEPT
    done
  fi
}

for spec in "$@"; do
  port=${spec%%:*}
  proto=${spec##*:}
  [ "$proto" = "$spec" ] && proto=tcp
  comment="$prefix-$port-$proto"
  apply_one iptables "$port" "$proto" "$comment"
  if command -v ip6tables >/dev/null 2>&1; then
    apply_one ip6tables "$port" "$proto" "$comment"
  fi
done
